use super::unregister_command;
use super::{
    flags::CommandFlags, get_command, register_command, registered_commands, Exec, Flag,
    ShellCommand,
};
use crate::utils::format_error;
use crate::{eval::Value, scope::Scope};
use std::any::Any;
use std::sync::Arc;

struct AliasRunner {
    args: Vec<String>,
}

impl AliasRunner {
    fn new(args: Vec<String>) -> Self {
        Self { args }
    }
}

impl Exec for AliasRunner {
    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }

    /// Execute alias command via the "eval" command.
    fn exec(&self, name: &str, args: &Vec<String>, scope: &Arc<Scope>) -> Result<Value, String> {
        let eval = get_command("eval").expect("eval command not registered");
        let combined_args: String = self
            .args
            .iter()
            .chain(args.iter())
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        eval.exec(name, &vec![combined_args], scope)
    }
}

struct Alias {
    flags: CommandFlags,
}

impl Alias {
    fn new() -> Self {
        let mut flags = CommandFlags::with_help();
        flags.add_flag('r', "remove", "Remove an existing alias");
        flags.add_flag('l', "list", "List all aliases");

        Self { flags }
    }

    fn add(&self, name: String, args: Vec<String>) -> Result<Value, String> {
        if get_command(&name).is_some() {
            Err(format!("{} already exists", name))
        } else {
            let runner = AliasRunner::new(args);
            register_command(ShellCommand {
                name,
                inner: Arc::new(runner),
            });
            Ok(Value::success())
        }
    }

    fn list(&self) {
        for name in registered_commands(true) {
            let cmd = get_command(&name).unwrap();

            match cmd
                .inner
                .as_ref()
                .as_any()
                .and_then(|any| any.downcast_ref::<AliasRunner>())
            {
                None => {}
                Some(runner) => {
                    println!("{}: {}", name, runner.args.join(" "));
                }
            }
        }
    }

    fn remove(&self, name: &str, scope: &Arc<Scope>, args: &[String]) -> Result<Value, String> {
        match get_command(name) {
            None => Err(format_error(scope, name, args, "alias not found")),
            Some(cmd) => {
                if cmd
                    .inner
                    .as_ref()
                    .as_any()
                    .and_then(|any| any.downcast_ref::<AliasRunner>())
                    .is_some()
                {
                    unregister_command(name);
                    Ok(Value::success())
                } else {
                    Err(format_error(scope, name, args, "not an alias"))
                }
            }
        }
    }
}

impl Exec for Alias {
    fn cli_flags(&self) -> Box<dyn Iterator<Item = &Flag> + '_> {
        Box::new(self.flags.iter())
    }

    fn exec(&self, _name: &str, args: &Vec<String>, scope: &Arc<Scope>) -> Result<Value, String> {
        let mut flags = self.flags.clone();
        let mut parsed_args = flags.parse_relaxed(scope, args);

        if flags.is_present("help") {
            println!("Usage: alias [NAME COMMAND [ARG...]] [OPTIONS]");
            println!("Register or deregister alias commands.");
            println!("\nOptions:");
            println!("{}", flags.help());
            println!("Examples:");
            println!("    alias la ls -al");
            println!("    alias --remove la");
            return Ok(Value::success());
        }

        if flags.is_present("list") {
            self.list();
            return Ok(Value::success());
        }

        if flags.is_present("remove") {
            if parsed_args.is_empty() {
                return Err("Please specify an alias to remove".to_string());
            }
            let name = &parsed_args[0];
            return self.remove(&name, scope, args);
        }

        // Register new alias
        if parsed_args.is_empty() {
            return Err("NAME not specified".to_string());
        }

        if parsed_args.len() < 2 {
            return Err("COMMAND not specified".to_string());
        }

        let name = parsed_args.remove(0);
        self.add(name, parsed_args)
    }
}

#[ctor::ctor]
fn register() {
    register_command(ShellCommand {
        name: "alias".to_string(),
        inner: Arc::new(Alias::new()),
    });
}
