use super::{flags::CommandFlags, register_command, Exec, Flag, ShellCommand};
use crate::{eval::Value, scope::Scope, symlnk::SymLink};
use regex::Regex;
use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::sync::Arc;

struct Find {
    flags: CommandFlags,
}

impl Find {
    fn new() -> Self {
        let flags = CommandFlags::with_help();
        Self { flags }
    }

    fn search(
        &self,
        scope: &Arc<Scope>,
        file_name: &OsStr,
        path: &Path,
        regex: &Regex,
    ) -> Result<(), String> {
        if Scope::is_interrupted() {
            return Ok(());
        }

        let search_path = path.dereference().unwrap_or(Cow::Owned(path.into()));

        // Check if the current directory or file matches the pattern
        if regex.is_match(&file_name.to_string_lossy()) {
            println!("{}", path.display());
        }

        if search_path.is_dir() {
            match fs::read_dir(search_path) {
                Ok(entries) => {
                    for entry in entries {
                        match entry {
                            Ok(entry) => {
                                self.search(scope, &entry.file_name(), &entry.path(), regex)?;
                            }
                            Err(e) => {
                                my_warning!(scope, "{}: {}", scope.err_path(path), e);
                            }
                        }
                    }
                }
                Err(e) => {
                    my_warning!(scope, "{}: {}", scope.err_path(path), e);
                }
            }
        }

        Ok(())
    }
}

impl Exec for Find {
    fn cli_flags(&self) -> Box<dyn Iterator<Item = &Flag> + '_> {
        Box::new(self.flags.iter())
    }

    fn exec(&self, _name: &str, args: &Vec<String>, scope: &Arc<Scope>) -> Result<Value, String> {
        let mut flags = self.flags.clone();
        let args = flags.parse(scope, args)?;

        if flags.is_present("help") {
            println!("Usage: find [OPTIONS] [DIRS...] PATTERN");
            println!("Recursively search and print paths matching PATTERN.");
            println!("\nOptions:");
            print!("{}", flags.help());
            return Ok(Value::success());
        }

        if args.is_empty() {
            return Err("Missing search pattern".to_string());
        }

        let pattern = args.last().unwrap(); // Last argument is the search pattern
        let regex = Regex::new(pattern).map_err(|e| format!("Invalid regex: {}", e))?;

        let dirs = if args.len() > 1 {
            &args[..args.len() - 1] // All except the last
        } else {
            &vec![String::from(".")] // Default to current directory
        };

        for dir in dirs {
            let path = Path::new(dir);
            self.search(scope, OsStr::new(dir), &path, &regex)?;
        }

        Ok(Value::success())
    }
}

#[ctor::ctor]
fn register() {
    register_command(ShellCommand {
        name: "find".to_string(),
        inner: Arc::new(Find::new()),
    });
}
