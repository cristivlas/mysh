use super::{flags::CommandFlags, register_command, Exec, ShellCommand};
use crate::{eval::Value, scope::Scope, symlnk::SymLink};
use std::path::Path;
use std::sync::Arc;

struct Realpath {
    flags: CommandFlags,
}

impl Realpath {
    fn new() -> Self {
        let flags = CommandFlags::with_follow_links();
        Self { flags }
    }
}

impl Exec for Realpath {
    fn exec(&self, _name: &str, args: &Vec<String>, scope: &Arc<Scope>) -> Result<Value, String> {
        let mut flags = self.flags.clone();
        flags.parse(scope, args)?;

        if flags.is_present("help") {
            println!("Usage: realpath [OPTION]... [FILE]...");
            println!("Print the canonicalized absolute path of each FILE.");
            println!("\nOptions:");
            print!("{}", flags.help());
            return Ok(Value::success());
        }

        if args.is_empty() {
            return Err("No arguments provided".to_string());
        }

        let follow = flags.is_present("follow-links");

        for (i, arg) in args.iter().enumerate() {
            scope.set_err_arg(i);
            let canonical_path = Path::new(arg)
                .resolve(follow)
                .and_then(|p| p.canonicalize())
                .map_err(|e| format!("{}: {}", scope.err_path_arg(arg, args), e))?;

            my_println!("{}", canonical_path.display())?;
        }

        Ok(Value::success())
    }
}

#[ctor::ctor]
fn register() {
    register_command(ShellCommand {
        name: "realpath".to_string(),
        inner: Arc::new(Realpath::new()),
    });
}
