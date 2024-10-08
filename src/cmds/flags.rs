use crate::{cmds::Flag, scope::Scope};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

#[derive(Clone)]
pub struct CommandFlags {
    flags: BTreeMap<String, Flag>,
    values: BTreeMap<String, String>,
    aliases: HashMap<String, String>, // Map aliases to the actual flag
    index: usize,
}

type ArgsIter<'a> = std::iter::Peekable<std::iter::Enumerate<std::slice::Iter<'a, String>>>;

impl CommandFlags {
    pub fn new() -> Self {
        CommandFlags {
            flags: BTreeMap::new(),
            values: BTreeMap::new(),
            aliases: HashMap::new(),
            index: 0,
        }
    }

    pub fn with_help() -> Self {
        let mut flags = Self::new();
        flags.add_flag('?', "help", "Display this help and exit");
        flags
    }

    pub fn with_follow_links() -> Self {
        let mut flags = Self::with_help();
        flags.add_flag_enabled('L', "follow-links", "Follow symbolic links");
        flags.add_alias(Some('P'), "no-dereference", "no-follow-links");

        flags
    }

    pub fn add(
        &mut self,
        short: Option<char>,
        long: &str,
        takes_value: Option<String>,
        help: &str,
    ) {
        self.add_with_default(short, long, takes_value, help, None)
    }

    pub fn add_alias(&mut self, short: Option<char>, alias: &str, other: &str) {
        // Get the "base name" i.e. strip negation for the base flag we are aliasing
        let (base_name, negated) = if other.starts_with("no-") {
            (&other[3..], true)
        } else {
            (other, false)
        };
        let flag = self.flags.get(base_name).expect("flag does not exist");
        if flag.takes_value.is_some() {
            panic!("Aliasing not supported for value flags");
        }
        if self
            .aliases
            .insert(alias.to_string(), other.to_string())
            .is_some()
        {
            panic!("Alias exists: {}", alias);
        }
        let help = if negated {
            format!("Do not {}", flag.help.to_lowercase())
        } else {
            flag.help.clone()
        };
        self.add(short, alias, None, &help);
    }

    pub fn add_with_default(
        &mut self,
        short: Option<char>,
        long: &str,
        takes_value: Option<String>,
        help: &str,
        default_value: Option<&str>,
    ) {
        if (short.is_some() && self.flags.values().find(|f| f.short == short).is_some())
            || self
                .flags
                .insert(
                    long.to_string(),
                    Flag {
                        short,
                        long: long.to_string(),
                        help: help.to_string(),
                        takes_value,
                        default_value: default_value.map(String::from),
                    },
                )
                .is_some()
        {
            panic!("flag {} (or its short form) already exists", long);
        }
    }

    /// Add boolean flag
    pub fn add_flag(&mut self, short: char, long: &str, help: &str) {
        self.add(Some(short), long, None, help);
    }

    pub fn add_flag_enabled(&mut self, short: char, long: &str, help: &str) {
        self.add_with_default(Some(short), long, None, help, Some("true"));
    }

    /// Add flag that takes a value
    pub fn add_value(&mut self, short: char, long: &str, name: &str, help: &str) {
        self.add(Some(short), long, Some(name.to_string()), help);
    }

    /// Parse command-line arguments and categorize them into flags and non-flag arguments.
    ///
    // Parameters:
    /// - `scope`: The current execution scope, wrapped in an `Arc` for future-proof thread-safety.
    /// - `args`: A slice of strings representing the command-line arguments to be parsed.
    ///
    /// Returns:
    /// - A `Result` containing a vector of non-flag arguments if parsing is successful,
    ///   or an error message as a string if parsing fails.
    pub fn parse(&mut self, scope: &Arc<Scope>, args: &[String]) -> Result<Vec<String>, String> {
        self.set_defaults();

        let mut args_iter = args.iter().enumerate().peekable();
        let mut non_flag_args = Vec::new();

        while let Some((i, arg)) = args_iter.next() {
            self.index = i;
            if arg.starts_with("--") && arg != "--" {
                self.handle_long_flag(scope, arg, &mut args_iter)?;
            } else if arg.starts_with('-') {
                if arg != "-" {
                    self.handle_short_flags(scope, arg, &mut args_iter)?;
                }
            } else {
                non_flag_args.push(arg.clone());
            }
        }

        Ok(non_flag_args)
    }

    /// Parse flags ignoring unrecognized flags.
    /// Useful when command needs to process arguments containing dashes, e.g. ```chmod a-w```
    /// and when passing commands to `run` and `sudo`.
    pub fn parse_relaxed(&mut self, scope: &Arc<Scope>, args: &[String]) -> Vec<String> {
        self.set_defaults();

        let mut args_iter = args.iter().enumerate().peekable();
        let mut non_flag_args = Vec::new();
        let mut encountered_double_dash = false;

        while let Some((i, arg)) = args_iter.next() {
            self.index = i;
            if encountered_double_dash || arg == "--" {
                encountered_double_dash = true;
                non_flag_args.push(arg.clone());
            } else if arg.starts_with("--") {
                if self.handle_long_flag(scope, arg, &mut args_iter).is_err() {
                    non_flag_args.push(arg.clone());
                }
            } else if arg.starts_with('-') && arg != "-" {
                if self.handle_short_flags(scope, arg, &mut args_iter).is_err() {
                    non_flag_args.push(arg.clone());
                }
            } else {
                non_flag_args.push(arg.clone());
            }
        }

        non_flag_args
    }

    fn set_defaults(&mut self) {
        for (k, f) in &self.flags {
            if let Some(value) = &f.default_value {
                self.values.insert(k.clone(), value.clone());
            }
        }
    }

    fn resolve_name(&self, name: &str) -> Option<(Flag, bool)> {
        match self.aliases.get(name) {
            Some(name) => self.resolve_name(name),
            None => {
                if let Some(flag) = self.flags.get(name) {
                    return Some((flag.clone(), false));
                }

                if name.starts_with("no-") {
                    if let Some((flag, _)) = self.resolve_name(&name[3..]) {
                        return Some((flag.clone(), true));
                    }
                }
                None
            }
        }
    }

    fn handle_long_flag(
        &mut self,
        scope: &Arc<Scope>,
        arg: &str,
        args_iter: &mut ArgsIter,
    ) -> Result<(), String> {
        if let Some((flag, is_negation)) = self.resolve_name(&arg[2..]) {
            if flag.takes_value.is_some() {
                if is_negation {
                    scope.set_err_arg(self.index);
                    return Err(format!(
                        "--no-{} is not valid for flag that takes a value",
                        flag.long
                    ));
                }
                if let Some((i, value)) = args_iter.next() {
                    self.index = i;
                    self.values.insert(flag.long.clone(), value.clone());
                } else {
                    scope.set_err_arg(self.index);
                    return Err(format!("Flag --{} requires a value", flag.long));
                }
            } else if is_negation {
                self.values.remove(&flag.long);
            } else {
                self.values.insert(flag.long.clone(), "true".to_string());
            }
        } else {
            scope.set_err_arg(self.index);
            return Err(format!("Unknown flag: {}", arg));
        }
        Ok(())
    }

    fn handle_short_flags(
        &mut self,
        scope: &Arc<Scope>,
        arg: &str,
        args_iter: &mut ArgsIter,
    ) -> Result<(), String> {
        let chars: Vec<char> = arg[1..].chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            if let Some(flag) = self.flags.values().find(|f| f.short == Some(c)) {
                let (flag, is_negation) =
                    self.resolve_name(&flag.long).expect("unknown short flag");

                if flag.takes_value.is_some() {
                    let value = if i + 1 < chars.len() {
                        // Case: -d2
                        chars[i + 1..].iter().collect::<String>()
                    } else if let Some((i, next_arg)) = args_iter.next() {
                        // Case: -d 2
                        self.index = i;
                        next_arg.clone()
                    } else {
                        scope.set_err_arg(self.index);
                        return Err(format!("Flag -{} requires a value", c));
                    };
                    // Special case -- consumes all flags
                    let value = if c == '-' {
                        std::iter::once(value)
                            .chain(args_iter.map(|(_, arg)| arg.clone()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    } else {
                        value
                    };

                    self.values.insert(flag.long.clone(), value);
                    break; // Exit the loop as we've consumed the rest of the argument
                } else if is_negation {
                    self.values.remove(&flag.long);
                } else {
                    self.values.insert(flag.long.clone(), "true".to_string());
                }
            } else {
                scope.set_err_arg(self.index);
                return Err(format!("Unknown flag: -{}", c));
            }
            i += 1;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    pub fn is_present(&self, name: &str) -> bool {
        #[cfg(not(test))]
        assert!(self.flags.contains_key(name));

        self.values.contains_key(name)
    }

    /// Returns an iterator over Flag values
    pub fn iter(&self) -> impl Iterator<Item = &Flag> {
        self.flags.values()
    }

    /// Query value, for flags that take value.
    pub fn value(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(|s| s.as_str())
    }

    pub fn help(&self) -> String {
        let mut help_text = String::new();

        for flag in self.flags.values() {
            let short_flag_help = if let Some(short) = flag.short {
                format!("-{}, ", short)
            } else {
                String::new()
            };
            let default_value_help = if let Some(ref default) = flag.default_value {
                format!(" (default: {})", default)
            } else {
                String::new()
            };

            let long_text = match &flag.takes_value {
                Some(name) => format!("{} <{}>", flag.long, name),
                None => flag.long.to_string(),
            };

            help_text.push_str(&format!(
                "{:4}--{:20} {}{}\n",
                short_flag_help, long_text, flag.help, default_value_help
            ));
        }
        help_text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn create_test_flags() -> CommandFlags {
        let mut flags = CommandFlags::new();
        flags.add_flag('v', "verbose", "Enable verbose output");
        flags.add_value('o', "output", "file", "Specify output file");
        flags.add_with_default(
            Some('d'),
            "debug",
            Some("level".to_string()),
            "Set debug level",
            Some("0"),
        );
        flags
    }

    #[test]
    fn test_default_values() {
        let mut flags = create_test_flags();

        let result = flags.parse(&Scope::new(), &vec![]);
        assert!(result.is_ok());
        assert_eq!(flags.value("debug"), Some("0"));
    }

    #[test]
    fn test_parse_long_flags() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec![
            "--verbose".to_string(),
            "--output".to_string(),
            "file.txt".to_string(),
        ];
        let result = flags.parse(&scope, &args);
        assert!(result.is_ok());
        assert!(flags.is_present("verbose"));
        assert_eq!(flags.value("output"), Some("file.txt"));
    }

    #[test]
    fn test_parse_short_flags() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec!["-v".to_string(), "-o".to_string(), "file.txt".to_string()];
        let result = flags.parse(&scope, &args);
        assert!(result.is_ok());
        assert!(flags.is_present("verbose"));
        assert_eq!(flags.value("output"), Some("file.txt"));
    }

    #[test]
    fn test_boolean_flag_negation() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec!["--no-verbose".to_string()];
        let result = flags.parse(&scope, &args);
        assert!(result.is_ok());
        assert!(!flags.is_present("verbose"));
    }

    #[test]
    fn test_boolean_flag_negation_after() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec!["--verbose".to_string(), "--no-verbose".to_string()];
        let result = flags.parse(&scope, &args);
        assert!(result.is_ok());
        assert!(!flags.is_present("verbose"));

        let args = vec![
            "--verbose".to_string(),
            "--no-verbose".to_string(),
            "--verbose".to_string(),
        ];
        let result = flags.parse(&scope, &args);
        assert!(result.is_ok());
        assert!(flags.is_present("verbose"));
    }

    #[test]
    fn test_invalid_negation() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec!["--no-output".to_string(), "file.txt".to_string()];
        let result = flags.parse(&scope, &args);
        assert!(result.is_err());
    }

    #[test]
    fn test_override_default_value() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec!["--debug".to_string(), "2".to_string()];
        let result = flags.parse(&scope, &args);
        assert!(result.is_ok());
        assert_eq!(flags.value("debug"), Some("2"));
    }

    #[test]
    fn test_help_output() {
        let flags = create_test_flags();
        let help_text = flags.help();
        assert!(help_text.contains("Enable verbose output"));
        assert!(help_text.contains("Specify output file"));
        assert!(help_text.contains("Set debug level (default: 0)"));
    }

    #[test]
    fn test_is_present() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec!["--verbose".to_string()];
        let result = flags.parse(&scope, &args);
        assert!(result.is_ok());
        assert!(flags.is_present("verbose"));
        assert!(!flags.is_present("output"));
    }

    #[test]
    fn test_parse_all() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec![
            "--verbose".to_string(),
            "--unknown".to_string(),
            "--output".to_string(),
            "file.txt".to_string(),
        ];
        let non_flag_args = flags.parse_relaxed(&scope, &args);
        assert!(flags.is_present("verbose"));
        assert_eq!(flags.value("output"), Some("file.txt"));
        assert_eq!(non_flag_args, vec!["--unknown"]);
    }

    #[test]
    fn test_short_flag_with_separate_value() {
        let mut flags = CommandFlags::new();
        let scope = Arc::new(Scope::new());

        // Adding flags for the test
        flags.add_value('d', "debug", "level", "Set debug level");

        // Test input: '-d' followed by '2' as a separate argument
        let args = vec!["-d".to_string(), "2".to_string()];

        // The parser should successfully parse the flag '-d' and assign the value '2'
        let result: Result<Vec<String>, String> = flags.parse(&scope, &args);

        assert!(result.is_ok(), "Expected the parser to succeed");
        assert_eq!(flags.value("debug"), Some("2"));
    }

    #[test]
    fn test_concatenated_short_flag_with_value() {
        let mut flags = CommandFlags::new();
        let scope = Arc::new(Scope::new());

        // Adding flags for the test
        flags.add_value('d', "debug", "level", "Set debug level");

        // Test input: '-d2', where '2' is concatenated with the flag '-d'
        let args = vec!["-d2".to_string()];

        // The parser should successfully parse the flag '-d' and assign the value '2'
        let result = flags.parse(&scope, &args);

        assert!(result.is_ok(), "Expected the parser to succeed");
        assert_eq!(flags.value("debug"), Some("2"));
    }

    #[test]
    fn test_parse_all_valid_flags() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec![
            "--verbose".to_string(),
            "--output".to_string(),
            "file.txt".to_string(),
            "--debug".to_string(),
            "2".to_string(),
        ];
        let non_flag_args = flags.parse_relaxed(&scope, &args);

        assert!(non_flag_args.is_empty(), "Expected no non-flag arguments");
        assert!(flags.is_present("verbose"));
        assert_eq!(flags.value("output"), Some("file.txt"));
        assert_eq!(flags.value("debug"), Some("2"));
    }

    #[test]
    fn test_parse_all_with_unknown_flags() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec![
            "--verbose".to_string(),
            "--unknown".to_string(),
            "-x".to_string(),
            "--output".to_string(),
            "file.txt".to_string(),
        ];
        let non_flag_args = flags.parse_relaxed(&scope, &args);

        assert_eq!(non_flag_args, vec!["--unknown", "-x"]);
        assert!(flags.is_present("verbose"));
        assert_eq!(flags.value("output"), Some("file.txt"));
    }

    #[test]
    fn test_parse_all_with_missing_value() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec!["--output".to_string()];
        let non_flag_args = flags.parse_relaxed(&scope, &args);

        assert_eq!(non_flag_args, vec!["--output"]);
        assert!(!flags.is_present("output"));
    }

    #[test]
    fn test_parse_all_with_mixed_valid_and_invalid() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec![
            "--verbose".to_string(),
            "--unknown".to_string(),
            "-o".to_string(),
            "file.txt".to_string(),
            "-x".to_string(),
            "--debug".to_string(),
            "2".to_string(),
            "non-flag-arg".to_string(),
        ];
        let non_flag_args = flags.parse_relaxed(&scope, &args);

        assert_eq!(non_flag_args, vec!["--unknown", "-x", "non-flag-arg"]);
        assert!(flags.is_present("verbose"));
        assert_eq!(flags.value("output"), Some("file.txt"));
        assert_eq!(flags.value("debug"), Some("2"));
    }

    #[test]
    fn test_parse_all_with_double_dash() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec![
            "--verbose".to_string(),
            "--".to_string(),
            "--output".to_string(),
            "file.txt".to_string(),
        ];
        let non_flag_args = flags.parse_relaxed(&scope, &args);

        assert_eq!(non_flag_args, vec!["--", "--output", "file.txt"]);
        assert!(flags.is_present("verbose"));
        assert!(!flags.is_present("output"));
    }

    #[test]
    fn test_parse_all_with_short_flags() {
        let mut flags = create_test_flags();
        let scope = Arc::new(Scope::new());
        let args = vec![
            "-v".to_string(),
            "-o".to_string(),
            "file.txt".to_string(),
            "-d2".to_string(),
        ];
        let non_flag_args = flags.parse_relaxed(&scope, &args);

        assert!(non_flag_args.is_empty(), "Expected no non-flag arguments");
        assert!(flags.is_present("verbose"));
        assert_eq!(flags.value("output"), Some("file.txt"));
        assert_eq!(flags.value("debug"), Some("2"));
    }

    #[test]
    fn test_alias() {
        let mut flags = CommandFlags::new();
        flags.add_flag('L', "follow", "Follow symbolic links");
        flags.add_alias(None, "deref", "follow");

        let result = flags.parse(&Scope::new(), &vec!["--deref".to_string()]);

        assert!(result.is_ok());
        assert!(flags.is_present("follow"));
        assert!(!flags.is_present("deref"));
    }

    #[test]
    fn test_alias_negate() {
        let mut flags = CommandFlags::new();
        flags.add_flag_enabled('L', "follow", "Follow symbolic links");
        flags.add_alias(None, "deref", "follow");

        let result = flags.parse(&Scope::new(), &vec!["--no-deref".to_string()]);
        assert!(result.is_ok());
        assert!(!flags.is_present("follow"));
        assert!(!flags.is_present("deref"));
    }

    #[test]
    fn test_alias_short_negate() {
        let mut flags = CommandFlags::new();
        flags.add_flag_enabled('L', "follow", "Follow symbolic links");
        flags.add_alias(Some('P'), "no-deref", "no-follow");

        let result = flags.parse(&Scope::new(), &vec!["-P".to_string()]);

        assert!(result.is_ok());
        assert!(!flags.is_present("follow"));
        assert!(!flags.is_present("deref"));
    }

    #[test]
    fn test_negate() {
        let mut flags = CommandFlags::new();
        flags.add_with_default(Some('m'), "messages", None, "", Some("true"));
        flags.add_alias(Some('s'), "silent", "no-messages");

        let result = flags.parse(&Scope::new(), &vec!["-s".to_string()]);

        assert!(result.is_ok());
        assert!(!flags.is_present("messages"));
        assert!(!flags.is_present("silent"));
    }
}
