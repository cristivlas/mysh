use super::{register_command, Exec, ShellCommand};
use crate::{eval::Value, scope::Scope};
use std::rc::Rc;
use std::sync::Arc;

struct Echo;

impl Exec for Echo {
    fn exec(&self, _name: &str, args: &Vec<String>, _: &Arc<Scope>) -> Result<Value, String> {
        my_println!("{}", args.join(" "))?;
        Ok(Value::success())
    }
}

#[ctor::ctor]
fn register() {
    register_command(ShellCommand {
        name: "echo".to_string(),
        inner: Rc::new(Echo),
    });
}
