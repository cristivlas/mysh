use cmds::{get_command, registered_commands, Exec};
use console::Term;
use directories::UserDirs;
use eval::{Interp, Value, KEYWORDS};
use prompt::PromptBuilder;
use rustyline::completion::{self, FilenameCompleter};
use rustyline::error::ReadlineError;
use rustyline::highlight::MatchingBracketHighlighter;
use rustyline::history::{DefaultHistory, SearchDirection};
use rustyline::{highlight::Highlighter, Context, Editor, Helper, Hinter, Validator};
use scope::Scope;
use std::borrow::Cow;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering::SeqCst},
    Arc,
};

use std::{env, usize};
use yaml_rust::Yaml;

#[macro_use]
mod macros;

mod cmds;
mod completions;
mod eval;
mod prompt;
mod scope;
mod symlnk;
mod testcmds;
mod testeval;
mod utils;

#[derive(Helper, Hinter, Validator)]
struct CmdLineHelper {
    #[rustyline(Completer)]
    completer: FilenameCompleter,
    #[rustyline(Highlighter)]
    highlighter: MatchingBracketHighlighter,
    scope: Arc<Scope>,
    completions: Option<Yaml>,
    prompt: String,
}

impl Highlighter for CmdLineHelper {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        default: bool,
    ) -> Cow<'b, str> {
        if default {
            Cow::Borrowed(&self.prompt)
        } else {
            Cow::Borrowed(prompt)
        }
    }

    fn highlight<'l>(&self, line: &'l str, pos: usize) -> Cow<'l, str> {
        self.highlighter.highlight(line, pos)
    }

    fn highlight_char(&self, line: &str, pos: usize, forced: bool) -> bool {
        self.highlighter.highlight_char(line, pos, forced)
    }
}

impl CmdLineHelper {
    fn new(scope: Arc<Scope>, completions: Option<Yaml>) -> Self {
        Self {
            completer: FilenameCompleter::new(),
            highlighter: MatchingBracketHighlighter::new(),
            scope: Arc::clone(&scope),
            completions,
            prompt: String::default(),
        }
    }

    fn keywords(&self) -> Vec<String> {
        registered_commands(false)
            .into_iter()
            .chain(KEYWORDS.iter().map(|s| s.to_string()))
            .collect()
    }

    // https://github.com/kkawakam/rustyline/blob/master/src/hint.rs#L66
    fn get_history_matches(&self, line: &str, pos: usize, ctx: &Context<'_>) -> HashSet<String> {
        let mut candidates = HashSet::new();
        let history_len = ctx.history().len();

        for index in (0..history_len).rev() {
            if let Ok(Some(sr)) = ctx.history().get(index, SearchDirection::Forward) {
                if sr.entry.starts_with(line) {
                    candidates.insert(sr.entry[pos..].to_owned());
                }
            }
        }

        candidates
    }

    fn set_prompt(&mut self, prompt: &str) {
        self.prompt = prompt.into()
    }
}

fn escape_backslashes(input: &str) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\\' {
            // Check if the next character is a backslash
            if chars.peek() == Some(&'\\') {
                // Keep both backslashes (skip one)
                result.push(c);
                result.push(chars.next().unwrap());
            } else {
                // Replace single backslash with double backslash
                result.push_str("\\\\");
            }
        } else {
            result.push(c);
        }
    }

    result
}

fn split_delim(line: &str) -> (&str, &str) {
    if let Some(pos) = line.rfind(&['\t', ' '][..]) {
        let head = &line[..pos + 1];
        let tail = line[pos..].trim();
        (head, tail)
    } else {
        ("", line)
    }
}

#[cfg(windows)]
/// The rustyline file auto-completer does not recognize WSL symbolic links
/// (because the standard fs lib does not support them). This function implements some
/// rudimentary support by matching the file_name prefix (not dealing with quotes and
/// escapes at this time).
fn match_path_prefix(word: &str, candidates: &mut Vec<completion::Pair>) {
    use crate::symlnk::SymLink;

    let path = std::path::Path::new(word);
    let mut name = path.file_name().unwrap_or_default().to_string_lossy();
    let cwd = env::current_dir().unwrap_or(PathBuf::default());
    let mut dir = path.parent().unwrap_or(&cwd).resolve().unwrap_or_default();

    if word.ends_with("\\") {
        if let Ok(resolved) = path.resolve() {
            if resolved.exists() {
                dir = resolved;
                name = std::borrow::Cow::Borrowed("");
            }
        }
    }

    if let Ok(read_dir) = &mut fs::read_dir(&dir) {
        for entry in read_dir {
            if let Ok(dir_entry) = &entry {
                let file_name = &dir_entry.file_name();

                if file_name
                    .to_string_lossy()
                    .to_lowercase()
                    .starts_with(name.as_ref())
                {
                    let display = if dir == cwd {
                        file_name.to_string_lossy().to_string()
                    } else {
                        if dir.starts_with(&cwd) {
                            dir = dir.strip_prefix(&cwd).unwrap_or(&dir).to_path_buf();
                        }

                        dir.join(file_name).to_string_lossy().to_string()
                    };

                    let replacement = if path.resolve().unwrap_or(path.to_path_buf()).is_dir() {
                        format!("{}\\", display)
                    } else {
                        display.clone()
                    };

                    candidates.push(completion::Pair {
                        display,
                        replacement,
                    })
                }
            }
        }
    }
}

#[cfg(windows)]
fn has_links(path: &Path) -> bool {
    use std::path::Component;

    let mut buf = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => continue,
            Component::ParentDir => {
                buf.pop();
            }
            _ => {
                buf.push(component);
            }
        }

        if buf.is_symlink() {
            return true;
        }
    }

    false
}

#[cfg(windows)]
fn match_symlinks(line: &str, word: &str, pos: &mut usize, candidates: &mut Vec<completion::Pair>) {
    if !word.is_empty() {
        if let Some(mut i) = line.to_lowercase().find(&format!(" {}", word)) {
            i += 1;

            // Check that the position is compatible with previous
            // completions before attempting to match path prefix
            if *pos == i || candidates.is_empty() {
                *pos = i;
                match_path_prefix(&line[i..], candidates);
            }
        }
    }
}

#[cfg(not(windows))]
fn has_links(_: &Path) -> bool {
    false
}

#[cfg(not(windows))]
fn match_symlinks(_: &str, _: &str, _: &mut usize, _: &mut Vec<completion::Pair>) {}

/// Provides autocomplete suggestions for the given input line using various strategies.
///
/// The method handles completion based on different scenarios:
///
/// - **History Expansion:** If the line starts with `!`, it expands history entries.
/// - **Environment Variable Expansion:** If the line contains `~`, it expands the `HOME` environment variable.
///   If the line contains `$`, lookup and expand the variable if it exists.
///
/// - **Keyword and Command Completion:** Completes keywords and built-in commands based on the input.
/// - **Custom Command Completions:** If no matches are found, it attempts to provide completions using custom configurations.
/// - **File Completion:** If all other completions fail, it resorts to file completions using `rustyline`'s built-in completer.
///
/// The function returns a tuple containing the position for insertion and a vector of candidates, or a `ReadlineError`
/// in case of failure.
impl completion::Completer for CmdLineHelper {
    type Candidate = completion::Pair;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        ctx: &Context<'_>,
    ) -> Result<(usize, Vec<Self::Candidate>), ReadlineError> {
        if pos < line.len() {
            return Ok((pos, vec![])); // Autocomplete only if at the end of the input.
        }
        // Expand !... TAB from history.
        if line.starts_with("!") {
            let candidates = self.get_history_matches(&line[1..], pos - 1, ctx);
            let completions: Vec<Self::Candidate> = candidates
                .into_iter()
                .map(|entry| Self::Candidate {
                    display: format!("{}{}", &line[1..], entry),
                    replacement: format!("{}{}", &line, entry),
                })
                .collect();

            return Ok((0, completions));
        }

        // Expand keywords and builtin commands.
        let mut keywords = vec![];
        let mut kw_pos = pos;

        let (head, tail) = split_delim(line);

        if tail.starts_with("~") {
            // TODO: revisit; this may conflict with the rustyline built-in TAB completion, which
            // uses home_dir, while here the value of $HOME is used (and the user can change it).
            if let Some(v) = self.scope.lookup("HOME") {
                keywords.push(completion::Pair {
                    display: String::default(),
                    replacement: format!("{}{}{}", head, v.value().as_str(), &tail[1..]),
                });
                kw_pos = 0;
            }
        } else if tail.starts_with("$") {
            // Expand variables
            kw_pos -= tail.len();
            keywords.extend(self.scope.lookup_starting_with(&tail[1..]).iter().map(|k| {
                Self::Candidate {
                    replacement: format!("${}", k),
                    display: format!("${}", k),
                }
            }));
        } else {
            let tok = head.split_ascii_whitespace().next();

            if tok.is_none() || tok.is_some_and(|tok| get_command(&tok).is_none()) {
                // Expand keywords and commands if the line does not start with a command.
                // TODO: expand command line flags for the builtin commands.
                kw_pos = 0;

                for kw in self.keywords() {
                    if kw.to_lowercase().starts_with(&tail) {
                        let repl = format!("{}{} ", head, kw);
                        keywords.push(completion::Pair {
                            display: repl.clone(),
                            replacement: repl,
                        });
                    }
                }
            }

            // Next try custom command completions
            if keywords.is_empty() {
                kw_pos = 0;

                if let Some(config) = &self.completions {
                    for completion in completions::suggest(config, line) {
                        keywords.push(completion::Pair {
                            display: completion.clone(),
                            replacement: completion,
                        });
                    }
                }
            }
        }

        // Handle (Windows-native and WSL) symbolic links using custom logic.
        match_symlinks(line, &tail, &mut kw_pos, &mut keywords);

        if keywords.is_empty() && !has_links(Path::new(&tail)) {
            // Try rustyline file completion next ...
            let completions = self.completer.complete(line, pos, ctx);

            if let Ok((start, v)) = completions {
                if !v.is_empty() {
                    // Replace unescaped \ with \\ in each completion's replacement
                    let escaped_completions: Vec<Self::Candidate> = v
                        .into_iter()
                        .map(|mut candidate| {
                            if tail.contains('"') || candidate.replacement.starts_with('"') {
                                candidate.replacement = escape_backslashes(&candidate.replacement);
                            }
                            candidate
                        })
                        .collect();

                    return Ok((start, escaped_completions));
                }
            }
        }

        Ok((kw_pos, keywords))
    }
}

type CmdLineEditor = Editor<CmdLineHelper, DefaultHistory>;

struct Shell {
    source: Option<Box<dyn BufRead>>,
    interactive: bool,
    wait: bool,
    interp: Interp,
    home_dir: Option<PathBuf>,
    history_path: Option<PathBuf>,
    profile: Option<PathBuf>,
    edit_config: rustyline::config::Config,
    prompt_builder: prompt::PromptBuilder,
    user_dirs: UserDirs,
}

/// Search history in reverse for entry that starts with &line[1..]
fn search_history<H: Helper>(rl: &Editor<H, DefaultHistory>, line: &str) -> Option<String> {
    let search = &line[1..];
    rl.history()
        .iter()
        .rev()
        .find(|entry| entry.starts_with(search))
        .cloned()
}

impl Shell {
    fn new() -> Result<Self, String> {
        #[cfg(not(test))]
        {
            ctrlc::set_handler(|| {
                INTERRUPT.store(true, SeqCst);
            })
            .expect("Error setting Ctrl+C handler");
        }

        let interp = Interp::new();
        let scope = interp.global_scope();

        let mut shell = Self {
            source: None,
            interactive: true,
            wait: false,
            interp,
            home_dir: None,
            history_path: None,
            profile: None,
            edit_config: rustyline::Config::builder()
                .edit_mode(rustyline::EditMode::Emacs)
                .behavior(rustyline::Behavior::PreferTerm)
                .completion_type(rustyline::CompletionType::List)
                .history_ignore_dups(true)
                .unwrap()
                .max_history_size(1024)
                .unwrap()
                .build(),
            prompt_builder: PromptBuilder::with_scope(&scope),
            user_dirs: UserDirs::new()
                .ok_or_else(|| "Failed to get user directories".to_string())?,
        };
        shell.set_home_dir(shell.user_dirs.home_dir().to_path_buf());

        Ok(shell)
    }

    /// Retrieve the path to the file where history is saved. Set profile path.
    fn init_interactive_mode(&mut self) -> Result<(&PathBuf, Option<Yaml>), String> {
        let mut path = self.home_dir.as_ref().expect("home dir not set").clone();

        path.push(".shmy");

        // Ensure the directory exists.
        fs::create_dir_all(&path)
            .map_err(|e| format!("Failed to create .shmy directory: {}", e))?;

        self.profile = Some(path.join("profile"));

        // Load custom completion file if present
        let compl_config_path = path.join("completions.yaml");
        let compl_config = if compl_config_path.exists() {
            Some(
                completions::load_config_from_file(&compl_config_path).map_err(|e| {
                    format!("Failed to load {}: {}", compl_config_path.display(), e)
                })?,
            )
        } else {
            None
        };

        // Set up command line history file
        path.push("history.txt");

        // Create the file if it doesn't exist
        if !path.exists() {
            File::create(&path).map_err(|e| format!("Failed to create history file: {}", e))?;
        }

        self.history_path = Some(path.clone());
        self.interp.set_var("HISTORY", path.display().to_string());

        Ok((self.history_path.as_ref().unwrap(), compl_config))
    }

    /// Populate global scope with argument variables.
    /// Return new child scope.
    fn new_top_scope(&self) -> Arc<Scope> {
        let scope = &self.interp.global_scope();
        // Number of args (not including $0)
        scope.insert(
            "#".to_string(),
            Value::Int(env::args().count().saturating_sub(1) as _),
        );
        // All args (not including $0)
        scope.insert(
            "@".to_string(),
            Value::Str(Arc::new(
                env::args().skip(1).collect::<Vec<String>>().join(" "),
            )),
        );
        // Interpreter process id
        scope.insert("$".to_string(), Value::Int(std::process::id() as _));
        // $0, $1, ...
        for (i, arg) in env::args().enumerate() {
            scope.insert(format!("{}", i), Value::Str(Arc::new(arg)));
        }

        Scope::new(Some(Arc::clone(&scope)))
    }

    fn read_lines<R: BufRead>(&mut self, mut reader: R) -> Result<(), String> {
        if self.interactive {
            println!("Welcome to shmy {}", env!("CARGO_PKG_VERSION"));

            // Set up rustyline
            let mut rl = CmdLineEditor::with_config(self.edit_config)
                .map_err(|e| format!("Failed to create editor: {}", e))?;

            let scope = self.interp.global_scope();
            let (history_path, completion_config) = self.init_interactive_mode()?;

            rl.set_helper(Some(CmdLineHelper::new(scope, completion_config)));
            rl.load_history(history_path).unwrap();

            self.source_profile()?; // source ~/.shmy/profile if found

            if !Term::stdout().features().colors_supported() {
                self.interp
                    .global_scope()
                    .insert("NO_COLOR".to_string(), Value::Int(1));
            } else {
                //
                // The `colored`` crate contains a SHOULD_COLORIZE singleton
                // https://github.com/colored-rs/colored/blob/775ec9f19f099a987a604b85dc72ca83784f4e38/src/control.rs#L79
                //
                // If the very first command executed from our shell is redirected or piped, e.g.
                // ```ls -al | cat```
                // then the output of the command does not output to a terminal, and the 'colored' crate
                // will cache that state and never colorize for the lifetime of the shell instance.
                //
                // The line below forces SHOULD_COLORIZE to be initialized early rather than lazily.
                //
                colored::control::unset_override();
            }

            // Run interactive read-evaluate loop
            while !self.interp.quit {
                let prompt = self.prompt_builder.prompt();

                // Hack around peculiarity in Rustyline, where a prompt that contains color ANSI codes
                // needs to go through the highlighter trait in the helper. The prompt passed to readline
                // (see below) causes the Windows terminal to misbehave when it contains ANSI color codes.
                rl.helper_mut().unwrap().set_prompt(&prompt);

                // Pass prompt without ANSI codes to readline
                let readline = rl.readline(&self.prompt_builder.without_ansi());

                match readline {
                    Ok(line) => {
                        if line.starts_with("!") {
                            if let Some(history_entry) = search_history(&rl, &line) {
                                eprintln!("{}", &history_entry);
                                // Make the entry found in history the most recent
                                rl.add_history_entry(&history_entry)
                                    .map_err(|e| e.to_string())?;
                                // Evaluate the line from history
                                self.eval(&history_entry);
                            } else {
                                println!("No match.");
                            }
                        } else {
                            rl.add_history_entry(line.as_str())
                                .map_err(|e| e.to_string())?;

                            self.save_history(&mut rl)?;
                            self.eval(&line);
                        }
                    }
                    Err(ReadlineError::Interrupted) => {
                        eprintln!("^C");
                    }
                    Err(err) => {
                        Err(format!("Readline error: {}", err))?;
                    }
                }
            }
        } else {
            // Evaluate a script file
            let mut script: String = String::new();
            match reader.read_to_string(&mut script) {
                Ok(_) => {
                    self.eval(&script);
                }
                Err(e) => return Err(format!("Failed to read input: {}", e)),
            }
        }
        Ok(())
    }

    fn save_history(&mut self, rl: &mut CmdLineEditor) -> Result<(), String> {
        let hist_path = self.history_path.as_ref().unwrap();
        rl.save_history(&hist_path)
            .map_err(|e| format!("Could not save {}: {}", hist_path.to_string_lossy(), e))
    }

    fn set_home_dir(&mut self, path: PathBuf) {
        let home_dir = path.to_string_lossy().to_string();
        self.home_dir = Some(path);
        self.interp.set_var("HOME", home_dir);
    }

    fn show_result(&self, scope: &Arc<Scope>, input: &str, value: &eval::Value) {
        use strsim::levenshtein;

        if input.is_empty() {
            return;
        }
        match value {
            Value::Str(s) => {
                println!("{}", s);

                if !input.contains(" ") {
                    let cmds = registered_commands(false);
                    if let Some((near, distance)) = cmds
                        .iter()
                        .map(|item| (item, levenshtein(item, s)))
                        .min_by_key(|&(_, distance)| distance)
                    {
                        if distance < std::cmp::max(near.len(), input.len()) {
                            eprintln!(
                                "{} was evaluated as a string. Did you mean '{}'?",
                                scope.err_str(input),
                                scope.err_str(near),
                            );
                        }
                    }
                }
            }
            _ => println!("{}", value),
        }
    }

    fn source_profile(&self) -> Result<(), String> {
        // Source ~/.shmy/profile if it exists
        if let Some(profile) = &self.profile {
            if profile.exists() {
                let scope = self.new_top_scope();
                let eval = get_command("eval").unwrap();
                eval.exec(
                    "eval",
                    &vec![profile.display().to_string(), "--source".to_string()],
                    &scope,
                )?;
            }
        }
        Ok(())
    }

    fn eval(&mut self, input: &String) {
        INTERRUPT.store(false, SeqCst);
        let scope = self.new_top_scope();

        match &self.interp.eval(input, Some(Arc::clone(&scope))) {
            Ok(value) => {
                // Did the expression eval result in running a command? Check for errors.
                if let Value::Stat(status) = &value {
                    if let Err(e) = &status.borrow().result {
                        e.show(&scope, input);
                    }
                } else if self.interactive {
                    self.show_result(&scope, &input.trim(), &value);
                }
            }
            Err(e) => {
                e.show(&scope, input);
                if !self.interactive && !self.wait {
                    std::process::exit(500);
                }
            }
        }
    }

    fn eval_input(&mut self) -> Result<(), String> {
        if let Some(reader) = self.source.take() {
            self.read_lines(reader)
        } else {
            panic!("No input source")
        }
    }
}

pub fn current_dir() -> Result<String, String> {
    match &env::current_dir() {
        Ok(path) => Ok(path.display().to_string()),
        Err(e) => Err(format!("Error getting current directory: {}", e)),
    }
}

fn parse_cmd_line() -> Result<Shell, String> {
    let mut shell = Shell::new()?;

    let args: Vec<String> = env::args().collect();
    for (i, arg) in args.iter().enumerate().skip(1) {
        if arg.starts_with("-") {
            if arg == "-c" || arg == "-k" {
                if !shell.interactive {
                    Err("Cannot specify -c command and scripts at the same time")?;
                }
                shell.source = Some(Box::new(Cursor::new(format!(
                    "{}",
                    args[i + 1..].join(" ")
                ))));
                shell.interactive = false;
                if arg == "-k" {
                    shell.wait = true;
                    shell
                        .interp
                        .global_scope()
                        .insert("NO_COLOR".to_string(), eval::Value::Int(1));
                }
                break;
            }
        } else {
            let file = File::open(&arg).map_err(|e| format!("{}: {}", arg, e))?;
            shell.source = Some(Box::new(BufReader::new(file)));
            shell.interactive = false;
            shell.interp.set_file(Some(Arc::new(arg.to_owned())));
        }
    }

    if shell.source.is_none() {
        shell.source = Some(Box::new(BufReader::new(io::stdin())));
    }

    Ok(shell)
}

static INTERRUPT: AtomicBool = AtomicBool::new(false);

fn main() -> Result<(), ()> {
    match &mut parse_cmd_line() {
        Err(e) => {
            eprint!("Command line error: {}.", e);
        }
        Ok(shell) => {
            match &shell.eval_input() {
                Err(e) => {
                    eprintln!("{}", e);
                }
                Ok(_) => {}
            }

            if shell.wait {
                prompt::read_input("\nPress Enter to continue... ").unwrap_or(String::default());
            }
        }
    }
    Ok(())
}
