use super::{flags::CommandFlags, register_command, Exec, ShellCommand};
use crate::{eval::Interp, eval::Value, scope::Scope};
use crate::{symlnk::SymLink, utils::format_error, utils::sync_env_vars};
use colored::*;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::rc::Rc;

struct Evaluate {
    flags: CommandFlags,
}

impl Evaluate {
    fn new() -> Self {
        let mut flags = CommandFlags::new();
        flags.add_flag('?', "help", "Display this help message");
        flags.add_flag('x', "export", "Export variables to environment");
        flags.add_flag('s', "source", "Treat the arguments as file paths");

        Self { flags }
    }
}

impl Exec for Evaluate {
    fn exec(&self, _name: &str, args: &Vec<String>, scope: &Rc<Scope>) -> Result<Value, String> {
        let mut flags = self.flags.clone();
        let eval_args = flags.parse(scope, args)?;

        if flags.is_present("help") {
            println!("Usage: eval EXPR...");
            println!("Evaluate each argument as an expression, stopping at the first error.");
            println!("\nOptions:");
            print!("{}", flags.help());
            return Ok(Value::success());
        }

        let export = flags.is_present("export");
        let source = flags.is_present("source");

        let mut interp = Interp::new();
        let global_scope = scope.global();

        for arg in &eval_args {
            let input = if source {
                // Treat arg as the name of a source file.
                // Resolve symbolic links (including WSL).
                let path = Path::new(&arg)
                    .resolve()
                    .map_err(|e| format_error(scope, arg, &args, e))?;

                let mut file = File::open(&path).map_err(|e| format_error(scope, arg, &args, e))?;

                let mut source = String::new(); // buffer for script source code

                file.read_to_string(&mut source)
                    .map_err(|e| format_error(scope, arg, &args, e))?;

                interp.set_file(Some(Rc::new(path.display().to_string())));

                source
            } else {
                interp.set_file(None);

                arg.to_owned()
            };

            match interp.eval(&input, Some(Rc::clone(&scope))) {
                Err(e) => {
                    e.show(scope, &input);
                    let err_expr = if scope.use_colors(&std::io::stderr()) {
                        arg.bright_cyan()
                    } else {
                        arg.normal()
                    };
                    return Err(format!("Error evaluating '{}'", err_expr));
                }

                Ok(value) => {
                    let mut command = false;
                    // Did the expression eval result in running a command? Check for errors.
                    if let Value::Stat(status) = &value {
                        if let Err(e) = &status.borrow().result {
                            return Err(e.to_string());
                        }
                        command = true;
                    }

                    if export {
                        // Export variables from the eval scope to the global scope
                        for (key, var) in scope.vars.borrow().iter() {
                            if !key.is_special_var() {
                                global_scope.insert(key.to_string(), var.value().clone());
                            }
                        }
                    } else if !command {
                        my_println!("{}", value)?;
                    }
                }
            }
        }

        if export {
            // Synchronize environment with global scope
            sync_env_vars(global_scope);
        }

        Ok(Value::success())
    }
}

#[ctor::ctor]
fn register() {
    register_command(ShellCommand {
        name: "eval".to_string(),
        inner: Rc::new(Evaluate::new()),
    });
}
